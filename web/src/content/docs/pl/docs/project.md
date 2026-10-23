---
title: 'Projekt i współtworzenie'
description: 'Stan Ku, publiczne źródła, granice licencji i odtwarzalne zgłoszenia problemów.'
kuVersion: '0.0.18 / experimental prerelease'
verifiedAt: '2026-10-20'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
  ]
---

Dokumentacja opisuje zamrożony zakres eksperymentalnego wydania wstępnego v0.0.18. Faktyczna publikacja i pliki do pobrania są zapisane w GitHub Releases. Projekt nosi nazwę **Ku programming language / Ku 编程语言**, a CLI nazywa się `ku`. Ku to mały język programowania i interpreter w rozwoju. Obecne publiczne wydanie to eksperymentalne wydanie wstępne v0.0.18.

## Źródła i licencjonowanie

Własny kod Ku w migawkach źródeł zawierających plik licencji oraz aktualne rozszerzenie edytora są udostępniane na [Apache License 2.0](https://github.com/fengyanweb/ku/blob/main/LICENSE) (`Apache-2.0`); informacje o autorstwie znajdują się w [NOTICE](https://github.com/fengyanweb/ku/blob/main/NOTICE). Zobacz [źródła na GitHubie](https://github.com/fengyanweb/ku) i [stronę wydania](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Komponenty stron trzecich zachowują własne licencje.

Archiwa v0.0.18-r1 i wcześniejszych wydań zachowują dołączone oryginalne materiały licencyjne; ta zmiana licencji źródeł nie powoduje ponownego pakowania tych archiwów. Używanie kompilatora Ku nie wymaga publikowania kodu źródłowego własnych programów. Przy rozpowszechnianiu programów zawierających kod środowiska uruchomieniowego Ku należy spełnić obowiązujące wymagania licencyjne i dotyczące autorstwa, w tym zachować informacje o komponentach stron trzecich.

## Zgłoś problem

Odtwarzalne zgłoszenia umieszczaj w [Issues](https://github.com/fengyanweb/ku/issues) repozytorium. Dołącz:

- Wersję Ku, commit Git dla źródeł rozwojowych oraz docelowy system / architekturę.
- Pełne polecenie, zaplecze i minimalne źródło `.ku`.
- Pełną diagnostykę lub ustrukturyzowany Error, oczekiwany i rzeczywisty wynik.
- Wersje narzędzi Rust / C przy problemach budowania oraz informację o lokalnych importach.

Nie przesyłaj tokenów, haseł, prawdziwych ciągów połączenia ani logów z poufnymi danymi biznesowymi. Raporty wydajności powinny zawierać środowisko, obciążenie, metodę pomiaru i surowe wyniki. Jedna udana próbka nie dowodzi przepustowości produkcyjnej.

## Przed współtworzeniem

[Kontrakt składni](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/syntax.md) i [kontrakt semantyki](https://github.com/fengyanweb/ku/blob/v0.0.18/docs/semantics.md) definiują zachowanie języka. Sprawdzaj CI, implementację i kontrakty łącznie. Cel w dzienniku prac nie jest zaimplementowaną funkcją.

Utrzymuj wąski zakres zmian i wyjaśniaj odtworzenie, przyczynę, zakres i weryfikację. Sprawdzaj treści strony razem z odpowiednim kodem. Zachowaj istniejące testy, zmiany drzewa roboczego i unikalne dowody. Opiekun określa przyjęcie wkładu i warunki licencji; witryna nie wymyśla umowy współtworzenia.

Ta witryna jest statyczna. Tekst, nawigacja i kod nie wymagają zaplecza biznesowego. Wyszukiwanie ładuje indeks Pagefind w przeglądarce; strona nie umożliwia zdalnego wykonywania kodu Ku.

Przejdź do [pierwszych kroków](/pl/docs/getting-started/), [granic obsługi](/pl/docs/support/) i [informacji o wydaniach](/pl/releases/).
