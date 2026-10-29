---
title: 'Obsługa platform i zapleczy'
description: 'Rozróżnij platformy wydań Ku, interpreter, program uruchamiający, natywne C, podzbiór Task i LLVM.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Pobieranie Ku 0.0.19

Ku 0.0.19 to eksperymentalna wersja wstępna. Dostępne są pakiety dla macOS ARM64 (Apple Silicon), Windows x64 i Linux x64 (GNU).

[Pakiety i instrukcja instalacji](/pl/docs/install/)

## Sposoby uruchamiania

- **`ku run`**: Interpreter języka z ogólną obsługą JSON i synchronicznymi handlerami HTTP; bez klientów baz danych.
- **`ku build`**: Domyślny runner osadza plik wejściowy; importy lokalne nadal wymagają plików źródłowych. Wymaga Rust **1.89.0** i systemowego linkera.
- **`ku build --backend c`**: Obsługuje synchroniczny JSON, HTTP z `http.json` oraz natywne klienty PostgreSQL, Redis i MySQL. Wymaga kompilatora C, linkera i odpowiednich bibliotek klienckich dla platformy docelowej. Operacje bazodanowe nie są automatycznie ponawiane.
- **Task**: Natywna ścieżka Task obsługuje typowany podzbiór: start/`await`, timery, wybrane operacje I/O, nazwane handlery async HTTP i określone zapytania PG. JSON w tej ścieżce pozwala tylko kodować literały obiektów o wartościach skalarnych; nie ma ogólnego dekodowania. Pełne M:N i ogólna obsługa I/O nie są jeszcze gotowe.
- **LLVM**: Tekstowy backend LLVM pozostaje ograniczonym prototypem.

## Znane problemy

Kontrole wydania i pakietów zakończyły się pomyślnie na wszystkich trzech platformach. Nie oznacza to ogólnego zatwierdzenia do produkcji.

- **HTTP 504:** występował wcześniej, lecz nie został odtworzony w najnowszej krótkiej kontroli; przyczyna pozostaje niepotwierdzona.
- **Skoki opóźnień podczas anulowania:** w najnowszej 30-sekundowej kontroli wszystkie 60 000 krótkich żądań były poprawne. p99 wyniosło **1.557 ms**, a maksimum **98.809 ms**; część żądań nadal przekroczyła 20 ms.

Formalny 30-minutowy test stabilności nie został jeszcze zaliczony. Krótkie pomiary go nie zastępują.

Pełna dokumentacja: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

Zgłaszanie problemów: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
