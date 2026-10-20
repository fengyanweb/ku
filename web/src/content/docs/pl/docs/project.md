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

Zobacz [źródła GitHub](https://github.com/fengyanweb/ku) i [stronę wydania](https://github.com/fengyanweb/ku/releases/tag/v0.0.18). Źródła można publicznie przeglądać, lecz własny kod Ku **nie został objęty udzieloną licencją open source**. Rozszerzenie edytora ma oznaczenie `UNLICENSED`. Publikacja kodu lub plików binarnych nie udziela takiej licencji automatycznie.

Informacje o komponentach zewnętrznych w archiwach dotyczą tylko odpowiednich komponentów. Nie ustanawiają uprawnień do używania, modyfikowania ani rozpowszechniania samego Ku. Wymagane upoważnienia muszą pochodzić wyraźnie od opiekuna; witryna nie tworzy nowych zobowiązań licencyjnych.

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
