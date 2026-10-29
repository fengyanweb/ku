---
title: 'Projekt i współtworzenie'
description: 'Stan Ku, publiczne źródła, granice licencji i odtwarzalne zgłoszenia problemów.'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'README.md',
    'Cargo.toml',
    'editors/vscode-ku/package.json',
    'docs/v0.0.18.md',
    'docs/syntax.md',
    'docs/v0.0.18-r1.md',
  ]
slug: pl/v0.0.18/docs/project
---

Ku to język programowania i zestaw narzędzi: interpreter, domyślny wykonywalny runner i natywna kompilacja C obsługiwanego podzbioru.

## Źródła i licencjonowanie

Zobacz [źródła GitHub](https://github.com/fengyanweb/ku) i [stronę wydania](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1). Źródła można publicznie przeglądać, lecz własny kod Ku **nie został objęty udzieloną licencją open source**. Rozszerzenie edytora ma oznaczenie `UNLICENSED`. Publikacja kodu lub plików binarnych nie udziela takiej licencji automatycznie.

Informacje o komponentach zewnętrznych w archiwach dotyczą tylko odpowiednich komponentów. Nie ustanawiają uprawnień do używania, modyfikowania ani rozpowszechniania samego Ku. Wymagane upoważnienia muszą pochodzić wyraźnie od opiekuna; witryna nie tworzy nowych zobowiązań licencyjnych.

## Zgłoś problem

Odtwarzalne zgłoszenia umieszczaj w [Issues](https://github.com/fengyanweb/ku/issues) repozytorium. Dołącz:

- Wersję Ku, commit Git dla źródeł rozwojowych oraz docelowy system / architekturę.
- Pełne polecenie, zaplecze i minimalne źródło `.ku`.
- Pełną diagnostykę lub ustrukturyzowany Error, oczekiwany i rzeczywisty wynik.
- Wersje narzędzi Rust / C przy problemach budowania oraz informację o lokalnych importach.

Nie przesyłaj tokenów, haseł, prawdziwych ciągów połączenia ani logów z poufnymi danymi biznesowymi. Raporty wydajności powinny zawierać środowisko, obciążenie, metodę pomiaru i surowe wyniki. Jedna udana próbka nie dowodzi przepustowości produkcyjnej.

## Przed współtworzeniem

Przy wkładzie podaj minimalne odtworzenie, zakres i odpowiednie testy. Kod i dokumentacja powinny dotyczyć tej samej wersji. Przyjęcie wkładu oraz prawa używania, modyfikacji i rozpowszechniania Ku uzgodnij z opiekunem.

Witryna nie umożliwia wykonywania kodu Ku online.

Przejdź do [pierwszych kroków](/pl/v0.0.18/docs/getting-started/), [granic obsługi](/pl/v0.0.18/docs/support/) i [informacji o wydaniach](/pl/v0.0.18/releases/).
