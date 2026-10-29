---
title: 'Suport pentru platforme și backenduri'
description: 'Deosebește platformele Ku publicate, interpretorul, lansatorul, C nativ, submulțimea Task și suportul LLVM.'
kuVersion: '0.0.19 / experimental prerelease'
verifiedAt: '2026-10-28'
sources:
  ['docs/v0.0.19.md', 'docs/concurrency.md', 'docs/v0.0.19-known-issues.md']
---

<span id="execution"></span>

## Descarcă Ku 0.0.19

Ku 0.0.19 este o versiune preliminară experimentală. Sunt disponibile pachete pentru macOS ARM64 (Apple Silicon), Windows x64 și Linux x64 (GNU).

[Pachete și instrucțiuni de instalare](/ro/docs/install/)

## Moduri de execuție

- **`ku run`**: Interpretor pentru limbaj, JSON general și handlere HTTP sincrone; fără clienți de baze de date.
- **`ku build`**: Runnerul implicit include fișierul de intrare; importurile locale au încă nevoie de fișierele sursă. Necesită Rust **1.89.0** și un linker de sistem.
- **`ku build --backend c`**: Acceptă JSON sincron, HTTP cu `http.json` și clienți nativi PostgreSQL, Redis și MySQL. Necesită un compilator C, un linker și biblioteci client compatibile cu ținta. Operațiile bazei de date nu sunt reîncercate automat.
- **Task**: Calea Task nativă acceptă un subset tipizat cu pornire/`await`, temporizatoare, anumite operații I/O, handlere HTTP async cu nume și anumite interogări PG. Pe această cale, JSON poate doar codifica obiecte literale cu valori scalare; decodificarea generală nu este disponibilă. M:N complet și acoperirea generală I/O nu sunt încă finalizate.
- **LLVM**: Backendul text LLVM rămâne un prototip limitat.

## Probleme cunoscute

Verificările publicării și ale pachetelor au trecut pe toate cele trei platforme. Acest lucru nu reprezintă o aprobare generală pentru producție.

- **HTTP 504:** a apărut în trecut, dar nu a fost reprodus în ultima reverificare scurtă; cauza rămâne neconfirmată.
- **Vârfuri de latență la anulare:** în ultima reverificare de 30 de secunde, toate cele 60.000 de cereri scurte au fost corecte. p99 a fost **1.557 ms**, iar maximul **98.809 ms**; unele cereri au depășit în continuare 20 ms.

Validarea formală a stabilității de 30 de minute nu a trecut încă. Măsurătorile scurte nu o înlocuiesc.

Referință completă: [English](/en/docs/support/) · [简体中文](/zh-cn/docs/support/)

Raportează probleme: [GitHub Issues](https://github.com/fengyanweb/ku/issues) · [kagura.gc@qq.com](mailto:kagura.gc@qq.com)
