---
title: 'Informacje o wydaniach'
description: 'Ku 0.0.18 — Informacje o wydaniach'
kuVersion: '0.0.18'
verifiedAt: '2026-10-23'
sources:
  [
    'docs/v0.0.18.md',
    'docs/v0.0.18-worklog.md',
    'docs/history.md',
    'Cargo.toml',
    'docs/v0.0.18-r1.md',
  ]
slug: pl/v0.0.18/releases
---

## v0.0.18

Poprawia ponowne obliczanie odbiorców w łańcuchach wywołań, natywne stałe float C i `time.date(Time): Date!`, aktualizując rozszerzenie VS Code.

- JSON: `encode`/`decode` współdzielą `stringify`/`parse`, z poprawką powrotnego odczytu `1e20`.

[Pobierz Ku 0.0.18](https://github.com/fengyanweb/ku/releases/tag/v0.0.18-r1) · [Obsługa platform i zapleczy](/pl/v0.0.18/docs/support/)

## v0.0.17 · Eksperymentalne wydanie wstępne

Główne zmiany:

- Synchroniczne pożyczone parametry tylko do odczytu używają `&name: T`, ze zwykłymi argumentami wywołania. Zwykłe parametry z własnością zachowują semantykę przenoszenia.
- Podzbiór parsera napisanego w Ku dodaje `while` z klamrami oraz ograniczonym zagnieżdżeniem i liczbą instrukcji.
- Archiwa zawierają pasujące zależności Rust domyślnego programu uruchamiającego i docelowy pakiet TLS. Nadal wymagane są właściwe narzędzia zewnętrzne.

Pliki do pobrania obejmują Windows x64, Linux x64 i macOS arm64. Siedem zasobów wydania to trzy archiwa platformowe, trzy manifesty i `SHA256SUMS`. Zobacz [instalację](https://github.com/fengyanweb/ku/releases/tag/v0.0.17) i [pełne informacje o wydaniu](https://github.com/fengyanweb/ku/releases/tag/v0.0.17).

### Zgodność i ograniczenia

Ku pozostaje eksperymentalne i nie jest gotowe do produkcji. Wersja 0.0.x nie obiecuje zgodności źródeł ani generowanego C/FFI ABI. Podczas aktualizacji przekompiluj odpowiednie natywne artefakty. Pożyczone parametry async, pełna własna implementacja i pełne natywne async nie należą do tego wydania.

Domyślny program uruchamiający osadza tylko źródło punktu wejścia; importowane moduły nadal potrzebują oryginalnych lokalizacji. Klienty baz są wyłącznie natywne. Sprawdź rzeczywisty serwer, TLS i ograniczenia stanu sesji każdego sterownika. Własny kod Ku nadal nie ma udzielonej licencji open source.
