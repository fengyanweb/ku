---
title: 'Projekt i współtworzenie'
description: 'Stan Ku, publiczne źródła, granice licencji i odtwarzalne zgłoszenia problemów.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.19.md',
    'docs/syntax.md',
  ]
---

Bieżąca dokumentacja dotyczy Ku 0.0.19, eksperymentalnego wydania wstępnego z tą samą wersją CLI i rozszerzenia VS Code. Dostępność, pliki i sumy kontrolne określa faktyczny GitHub Release.

**Ku programming language / Ku 编程语言** · CLI: `ku`.

## Źródła i licencjonowanie

[GitHub](https://github.com/fengyanweb/ku) · [v0.0.19 Release](https://github.com/fengyanweb/ku/releases/tag/v0.0.19)

Ku i rozszerzenie VS Code są dostępne na licencji Apache License 2.0. Zobacz [LICENSE](https://github.com/fengyanweb/ku/blob/main/LICENSE) i [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE). Składniki firm trzecich zachowują własne prawa autorskie i licencje.

## Zgłoś problem

Odtwarzalne zgłoszenia umieszczaj w [Issues](https://github.com/fengyanweb/ku/issues) repozytorium. Dołącz:

- Wersję Ku, commit Git dla źródeł rozwojowych oraz docelowy system / architekturę.
- Pełne polecenie, zaplecze i minimalne źródło `.ku`.
- Pełną diagnostykę lub ustrukturyzowany Error, oczekiwany i rzeczywisty wynik.
- Wersje narzędzi Rust / C przy problemach budowania oraz informację o lokalnych importach.

Nie przesyłaj tokenów, haseł, prawdziwych ciągów połączenia ani logów z poufnymi danymi biznesowymi. Raporty wydajności powinny zawierać środowisko, obciążenie, metodę pomiaru i surowe wyniki. Jedna udana próbka nie dowodzi przepustowości produkcyjnej.

## Przed współtworzeniem

[Kontrakt składni](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/syntax.md) i [kontrakt semantyki](https://github.com/fengyanweb/ku/blob/v0.0.19/docs/semantics.md) definiują zachowanie języka. Sprawdzaj CI, implementację i kontrakty łącznie. Cel w dzienniku prac nie jest zaimplementowaną funkcją.

Utrzymuj wąski zakres zmian i wyjaśniaj odtworzenie, przyczynę, zakres i weryfikację. Sprawdzaj treści strony razem z odpowiednim kodem. Zachowaj istniejące testy, zmiany drzewa roboczego i unikalne dowody. Opiekun określa przyjęcie wkładu i warunki licencji; witryna nie wymyśla umowy współtworzenia.

Ta witryna jest statyczna. Tekst, nawigacja i kod nie wymagają zaplecza biznesowego. Wyszukiwanie ładuje indeks Pagefind w przeglądarce; strona nie umożliwia zdalnego wykonywania kodu Ku.

Przejdź do [pierwszych kroków](/pl/docs/getting-started/), [granic obsługi](/pl/docs/support/) i [informacji o wydaniach](/pl/releases/).
